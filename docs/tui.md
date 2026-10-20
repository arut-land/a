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
terminal thread ──Input / Resized──┐
 (poll: stdin + signal pipe)       ├─> inbox ─> event loop: App::event / App::input
client::Driver ──client::Event─────┘               │   (SessionClient folds the view)
 (a reader thread per connection)                  ▼
        App::draw ─> Canvas (a cell grid, redrawn by the row) ─> Screen: changed cells as ANSI, one write
```

The daemon side is the client SDK's (docs/client.md): `App` holds a
`SessionClient`, hands it the driver's events and the person's commands,
and renders the effects it gets back; what the TUI adds is screen state.

Everything under the view is ours and small. The text model (`text.rs`):
a `Style` is the pen (a palette color, attributes: 3 bytes), a `Span` is a
`SmolStr` (inline up to 23 bytes, shared past that) and a pen, a `Line` is
up to four spans inline; a row clones without allocating. The grid
(`render.rs`): a `Cell` is one character with the zero-width marks that
follow it and its pen; `Grid::put` writes a cell a character, as `wrap`
counted them. The terminal layer: `term.rs` (raw mode through rustix
termios, the modes, restoring them, the input thread), `keys.rs` (input
bytes to keys on vte) and `screen.rs` (cells to ANSI). And what a chat
transcript needs and no widget gives: wrapping cached per item,
virtualized scrolling from an anchor, clickable spans, and the input
editor. `ratatui-core` was the grid until October 2026; it came with ten
crates, three of them proc-macros, for `Buffer`, `Cell` and `Style`.

- One thread owns the app and blocks on its `client::Inbox`. The terminal
  thread blocks in `poll` on stdin and a signal pipe with no timeout; the
  `Driver` reads each connection on a thread of its own. Nothing polls on
  a timer. Everything queued is folded before one frame; what the terminal
  thread posts (`Post::send_first`) is taken before the daemon's backlog,
  so a keystroke or a resize is folded ahead of a replay still queued.
- Signals (`sys::Signals`, the self-pipe trick: the handler writes one byte)
  reach that same `poll`: SIGWINCH is a resize, SIGTERM and SIGHUP restore
  the terminal and then end the process as the signal would have
  (`sys::reraise`). They are caught before the terminal goes raw, and
  read for as long as the process lives, stdin gone or not. The mode is
  restored first; the escapes that leave the screen are given half a
  second, since a terminal nobody reads blocks every write, and must not
  outlive a SIGTERM.
- Input: vte decodes characters (UTF-8 split across reads included),
  controls and CSI sequences; `keys.rs` tells an ESC prefix (Alt, Alt-Enter)
  and SS3 apart itself, since vte would read either as the start of a
  sequence. An ESC that ends a read with nothing more waiting is the Esc
  key. Keys include xterm's modifier parameters, `CSI u` and
  modifyOtherKeys for Enter with a modifier; bracketed paste; SGR mouse
  clicks and wheel.
- `App::draw` draws rows as lines onto its `Canvas` (from row 0) and returns
  the caret. A row drawn with the same lines as the frame before keeps its
  cells: nothing is put or compared there. Only rows whose lines changed
  are redrawn and compared cell by cell with what they held, so a streamed
  token costs the row it changed. The canvas also says where each
  clickable span landed (an overlay makes the transcript's inert).
- `Screen` writes the changed cells: cursor moves only where the next cell
  is not where the cursor is, one SGR per style change, inside a
  synchronized update (`?2026`), as one `write(2)` per frame. The event
  loop draws only when the app owes a frame (`App::needs_frame`: a view
  change, a key, the link moving), so an ack, or a session list while no
  picker is open, is not even composed; a frame that still changed no cell
  and moved no cursor writes nothing, not even the envelope.
- The rows under the transcript (recap, approval bar, notice, input,
  status) and the overlay are composed once per frame: inline, `height`
  composes them to size the viewport and `draw` draws the same ones.
- Full screen: the alternate buffer, addressed absolutely, with click and
  wheel reports (SGR) only (`?1000`, `?1006`; not every motion, which would
  wake the client as the pointer moves).
- Inline, as Codex does it: a viewport at the bottom of the normal buffer.
  Its rows are reached by moves relative to the cursor, which stays where
  the last frame left it (the caret, else the last row), so the terminal is
  never asked where the cursor is. It grows by printing newlines (scrolling
  the screen at the bottom) and moving back up; settled items are printed
  from its top with `\r\n`, so they scroll into the terminal's own
  scrollback, and the viewport is drawn whole under them. A resize or
  Ctrl-L draws it whole from its top.
- Rows are wrapped lazily and cached per item and width: a frame wraps only
  what it shows, counted from the bottom or from the scroll anchor (an item
  and a row, which stays put as output arrives below). A 6000-item session
  costs about what a short one does to draw, resize or resume. A reply
  streaming in keeps the rows of its finished lines; a delta wraps only the
  line still open. What the bottom rows need of the whole transcript (the
  oldest ask, whether a turn runs, the latest model) is found once per
  change, not per frame.
- Text goes into the grid a cell a character, the columns `wrap` counted
  for it. A zero-width character (a combining mark, a variation selector)
  joins the cell before it; joiners and format characters are dropped,
  since the terminal would otherwise advance fewer columns than the grid
  did.
- The terminal is restored on exit, on a panic (a hook), when stdin
  closes, and on SIGTERM or SIGHUP.
- The first frame is painted before the daemon is reached and before the
  repository root is walked up to for the picker's default; connecting
  (and spawning the daemon) happens on the reader thread. An ssh endpoint
  connects first, since ssh may prompt on the terminal.
- Unix only, like the daemon: on Windows `tui` builds to a stub and `a`
  runs the line REPL.

## The terminal layer: measurements

### The own grid and allocation-free rows

20 October 2026, release profile with `build-std` (so the `alloc` level
applies), Apple M3 under load, `render_cost` (6000 items, 120×40) in turns:
before is `ratatui-core` with `String` spans and `alloc` at 3, after is the
own grid with `SmolStr` spans and inline rows. Ranges are five runs each, in
µs; the two `alloc` levels were run alternately, seven rounds.

| | before, alloc 3 | after, alloc 3 | after, alloc s |
| --- | --- | --- | --- |
| first frame | 289–330 | 191–216 | 199–234 |
| delta frame p50 / p99 | 35–51 / 117–161 | 20–24 / 94–109 | 21–24 / 97–109 |
| scroll frame p50 / p99 | 80–101 / 104–184 | 68–83 / 83–103 | 75–80 / 90–98 |
| re-wrap all (resize) | 240–316 | 156–184 | 170–186 |
| allocations, delta (fold and frame) / idle frame | not counted | 47 / 16 | 47 / 16 |
| bytes written, delta / idle frame | 222 / 16 | 222 / 0 | 222 / 0 |

So `alloc` builds at s with everything else (`.cargo/profiles.toml` has
one release level and `serde_json`): the two levels are indistinguishable
on every row. What an idle frame still allocates is the bottom rows being
formatted (status, approval bar, editor rows) and the transcript window;
the canvas finds them unchanged and nothing is written.

| | before | after |
| --- | --- | --- |
| Release `a` (`cargo build-release -p a`) | 2845 KiB | 2797 KiB (`alloc` at s alone: 2780) |
| `mise run size` | 2829 KiB | 2813 KiB |
| Packages in `cargo tree -p a` (host) | 141 | 120 |
| Proc-macro crates in that graph | 9 | 6 |
| Idle client RSS (budgets) | 7.5–7.6 MB | 7.4 MB |

Gone: ratatui-core, kasuari, lru, strum, strum_macros, thiserror,
thiserror-impl, compact_str, castaway, rustversion, unicode-truncate,
unicode-segmentation, itertools, foldhash. Came (both linked already):
smol_str, smallvec. `syn 2` stays for `hypertext-macros` alone.

### Before that: the terminal layer

October 2026, release profile (fat LTO, one codegen unit, `build-std`,
stripped), Apple M3, `nightly-2026-10-17`, on a loaded machine.
Before is crossterm 0.29 with `ratatui-crossterm` (ratatui's `Terminal`:
a reset and a full diff of the buffer every frame) and signal-hook; after
is the layer above. `render_cost` is
`cargo test --release -p tui -- --ignored --nocapture render_cost`
(6000 items, 120×40, frames include encoding the bytes), medians of nine
runs of the six builds in turns, in µs:

| tui at | first frame | delta p50 / p99 | scroll p50 / p99 | re-wrap all | bytes / delta |
| --- | --- | --- | --- | --- | --- |
| before, 3 | 264 | 102 / 167 | 133 / 173 | 234 | 286 |
| before, s | 294 | 141 / 235 | 150 / 196 | 300 | 286 |
| before, z | 365 | 198 / 343 | 204 / 262 | 432 | 286 |
| after, 3 | 257 | 36 / 118 | 74 / 93 | 199 | 222 |
| after, s | 288 | 38 / 115 | 80 / 104 | 226 | 222 |
| after, z | 381 | 72 / 230 | 149 / 193 | 339 | 222 |

So `tui` builds at s (it was the one package at 3). What a delta frame
still costs is re-wrapping the open line of the reply (here 500 tokens in
one paragraph). Wrapping cloned a span's whole text for every word it
split off, quadratic in a long paragraph; it no longer does.

| | before | after |
| --- | --- | --- |
| Release binary (`mise run size`) | 3108 KiB | 3011 KiB (−97) |
| `__text` (`size -m`) | 2214 KiB | 2124 KiB |
| Packages in `cargo tree -p a` (host / all targets) | 187 / 234 | 166 / 208 |
| Idle client RSS (budgets) | 7.6–7.7 MB | 7.5–7.6 MB |
| Idle client CPU (3 s) | 0 | 0 |
| Spawn to first frame, full screen (pty, median of 5, three rounds) | 13.0 / 6.4 / 12.0 ms | 12.8 / 7.1 / 11.0 ms |
| Spawn to first frame, inline, terminal answers | 8.9–24.8 ms | 7.5–11.9 ms |
| Spawn to first frame, inline, terminal never answers | 509 ms | 7.2 ms |

Gone: crossterm, crossterm_winapi, ratatui-crossterm, signal-hook,
signal-hook-mio, signal-hook-registry, parking_lot, parking_lot_core,
lock_api, scopeguard, derive_more, convert_case, darling, ident_case,
strsim, document-features, litrs, indoc, instability, winapi and
redox_syscall (mio went with tokio once the web UI moved onto `rt`;
`mise run check:no-tokio` keeps both out). What replaced
them, on rustix and vte (which `a` linked already): `keys.rs`, `screen.rs`
and `term.rs` are 830 lines without their tests (570 on crossterm), and
`sys::Signals` 140.

### Before that: choosing a library

A prototype chat client (2000-message transcript, status, input, wheel
scroll, a stream of 1000 deltas, blocking input plus a channel; tui at
opt-level s) on a 120×40 pty, against the same program printing lines:

| | Binary | vs base | Idle RSS | Idle CPU | CPU, 1000 deltas | First frame (in process) |
| --- | --- | --- | --- | --- | --- | --- |
| base (lines) | 256 KiB | | 2.5 MB | 0 | 0.19 s | 1.7 ms |
| ratatui-core + crossterm 0.29 | 457 KiB | +201 KiB | 3.0 MB | 0 | 0.22 s | 1.9 ms |
| ratatui-core + termion 4 | 413 KiB | +157 KiB | 3.0 MB | 0 | 0.21 s | 1.3 ms |
| ratatui-core + termina 0.3 | 428 KiB | +172 KiB | 3.0 MB | 0 | 0.16 s | 0.7 ms |

- crossterm 0.29.0 (April 2025): the de facto backend, used by Codex;
  Windows console and VT; brings mio, signal-hook and parking_lot on Unix.
- termion 4: Unix only, no resize event.
- termina (helix): about 30 KiB smaller, no mio, Windows via VT; but
  `ratatui-termina` 0.1.0 pins termina 0.3 while termina is at 0.4.
- Frameworks (iocraft, cursive, tui-realm, r3bl_tui, rat-salsa) bring
  layout engines, async runtimes or no inline mode; ratatui-textarea does
  not soft-wrap, which the input needs.

None of them is needed once the client is Unix only and draws its own
diff: the backends' work (raw mode, a key decoder, a cursor-move and SGR
encoder) is what rustix and vte, linked already, leave to write.

## Tests

- Unit tests draw the app onto its canvas and read the grid's rows
  (`app/tests.rs`, `view.rs`, `text.rs`, `editor.rs`, `render.rs`):
  approvals by key and click, streaming, folding, scrolling with an
  anchor, slash commands, the picker, the recap, the inline scrollback,
  tiny sizes. Every test frame is also drawn on a blank canvas and must
  match: the rows a frame kept are the ones it would have drawn.
- The frame invariants, on a terminal that counts `write(2)` calls, over
  a session's frames in both viewports: `one_write_per_frame`,
  `an_idle_frame_writes_nothing`, and `no_terminal_queries` (every byte
  written is a cursor move, an erase, an SGR or a mode: nothing the
  terminal would answer). `first_frame_before_the_daemon_answers`
  (`apps/cli/tests/blackbox/tui.rs`) holds the daemon's socket with a listener that
  never answers and sees the frame; the pty harness answers no query.
- `keys.rs` decodes byte sequences (keys with modifiers, Alt, SS3, F-keys,
  `CSI u`, UTF-8 and sequences split across reads, mouse reports, pastes);
  `screen.rs` checks the SGR it writes; `sys` checks a caught signal
  arrives on the pipe.
- `apps/cli/tests/blackbox/tui.rs` runs `a` on a pseudo-terminal against an echo
  agent daemon in its own `A_HOME`, reads the screen through a small
  terminal model (`tests/common/pty.rs`), and checks a reply, an approval
  answered by a click, the command's result, detaching, an inline
  `a resume` that prints the replay into the scrollback, and a SIGTERM
  that leaves the terminal cooked.
- `apps/cli/tests/blackbox/budgets.rs` (`mise run budgets`, release build)
  holds the first frame to 16 ms and an idle client to no CPU.

## Limits

- Wrapping and drawing count columns per character, so an emoji ZWJ
  sequence shows as its parts (`👨‍👩‍👧` as three faces), as wide as it was
  counted.
- SIGKILL leaves the terminal in raw mode (`reset` fixes it).
- Ctrl-Z is not handled (raw mode receives it as a key).
- Inline, a terminal that reflows its lines when narrowed may leave a
  copy of a reflowed viewport row above the redrawn viewport (as before,
  when the client asked the terminal where the cursor was).
- An ESC split from the rest of its sequence by a read boundary, with
  nothing more waiting, is the Esc key.
- The full screen reports the mouse, so selecting text takes the terminal's
  override (Shift-drag in most, Option-drag in iTerm2 and Terminal.app);
  the inline mode leaves the mouse to the terminal.
