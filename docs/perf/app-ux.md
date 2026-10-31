# app-ux: phase 1

Child operations render once and open their own paged transcript in macOS,
web and TUI. All three clients now have a conversation switcher below the
composer, including keyboard navigation, parent return, child messaging and
stop through the existing interrupt path. Native scrolling follows the actual
viewport bottom and retains its visible anchor across external updates and
adjacent history pages.

The native memory and cold-row follow-ups are included in the final measurements
below. The owner's white-space screenshot remains pending; that symptom has not
been attributed or claimed fixed. Phase 1 did not dispatch full CI. The phase-2 review fixes and final CI result
are recorded at the end.

## Reproduction and root causes

The baseline is `5610d9e021bbf83044a80974c7f6eac147fa2564`, PROTO 43,
with the release native app, release fixture-enabled helper and isolated
real daemon using the echo agent. Short, large, parallel, fork and background
spawns were exercised without a provider request.

- A short spawn produced a tool row at key 4 and a child row at key 5 for
  one operation. Two parallel spawns produced two tool rows and two child
  rows. The keys were distinct; this was duplicate presentation, not duplicate
  Swift identifiers. The shared reducer independently appended ToolCall and
  Link. Demand paging faithfully reconstructed both, rather than creating
  the duplicate. This does not establish which historical merge introduced it.
- The large child produced a real spilled tool result: 10,828 bytes, including
  the task wrapper. `Shown::Held` turned its reference into user-facing MIME,
  blob terminology and an identifier. No evidence showed a context-graph
  authority image escaping into the transcript.
- The macOS item renderer ignored the existing `DisplayItem.navigation`.
  The real web event stream and real TUI attach process also showed the
  tool/child pair and storage terms. This located the shared defects below
  all three renderers.
- Native scrolling upward left `followsHistory` true, since that flag describes
  the requested SDK range. `FollowEnd` called `scrollTo` on every revision.
  An external writer moved clip origin from 6,388 to 9,405 points while the
  reader was above the bottom. The old paging callback chose a new page end
  instead of preserving a visible row and pixel offset.

The simultaneous baseline FFI/store dumps, native view hierarchy and in-process
window capture are retained alongside the original PTY and web output.

## Shared changes and conversation navigation

`Link.call` records the owning spawn decision's CallId, as approved. The kernel
already owns that irreducible fact. It is the only new durable field. The shared
view folds the tool call and child link into one typed Child item, preserving the
operation key; additional children of one call retain their link keys. Watches
update status and result summaries without recreating tool cards. The indexed
history retains contributing facts for reconstruction at old cuts. Input recall
retains the original editable command independently of its child presentation.

Stored output now presents `Output · N bytes` and **Open output**. Its reference
stays a retrieval parameter. The native sheet, terminal output view and
authenticated web output route fetch it on demand. Real browser and FFI fetches
both returned the complete 10,828-byte fixture output. Four real task/fork/
background/large operations produced four Child items and zero duplicate tool
rows. Baseline TUI and web had the same duplication/storage-label defect; the
shared fix applies to them too.

Public `Head.conversation` projects existing parent, spawn kind/title/admission,
execution spans and outcome. No new timing or tree facts are logged; no private
spawn authority reaches clients. The approved driver edits only pass existing
parent identity to the head publisher. PROTO 44 introduced Link.call; PROTO 45
adds the public head metadata. Wire corpora and shared vectors were generated
by their scripts/tests, without rewriting old recordings.

The SDK's `ConversationTree` and `ConversationWatch` supply one family list,
independent of demanded transcript pages. Public heads/activity supply status,
elapsed time and token counts. Completed duration sums observed monotonic spans
within each clock epoch, including gaps between calls but excluding invented
restart downtime. An unobserved duration displays `—`; the echo fixture has no
real provider timing or token usage. Running families refresh once per second.
Settled observers release their connections/history; catalog revisions trigger
head-only reattachment when a settled session changes. Finished children remain
in the list, including after subsequent child inbox messages. Complete FFI list
snapshots buffer only the latest list, and identical family snapshots are not
republished.

Empty-composer Down enters the list; arrows select; Return/Enter opens a session;
Escape or Up past the first row returns to composition. x stops a running child
through the existing cancel path and reports normal errors. TUI uses the same
model in alternate-screen and main-buffer modes and supports existing mouse
row actions. macOS uses a native NSTableView with current-conversation markers;
web uses stable accessible options and roving tabindex, preserving focused-row
hints through live updates. A child composer shows `Message @<title>…` and sends
through its ordinary inbox. The list and explicit parent link return to the
parent. macOS omits the current family's children from its sidebar because they
already have their dedicated conversation list; unrelated sessions remain.

## Native scrolling, memory and first layout

NSScrollView clip bounds determine bottom pinning. NSTableView row rectangles,
stable item/fragment IDs and scalar height caches retain a visible item and pixel
offset through page installation. The preceding page's display models can remain
during delivery without expanding SDK demand. There is no per-revision scrollTo.
The real native regression scrolls up, receives an external writer update and
asserts unchanged clip origin. It also checks earlier/later page anchors and
following at the live bottom. Its paging assertions were not weakened.

The initial eager SwiftUI page improved scrolling but added 30.906 MiB RSS.
Live heap/vmmap evidence attributes it to laid-out text and layers: CoreText
line/run counts each rose from 121 to 2,230, attributed strings from 248 to
2,397, CoreAnimation resident dirty from 2,784 KiB to 12.3 MiB, and allocated
heap from 23.9 to 35.1 MiB. Images stayed at 33 with 384 KiB of CoreUI data;
NSTextStorage was one 640-byte object in both. Physical footprint rose from
50.6 to 73.2 MiB. These diagnostic captures are separate from timing rounds;
physical footprint, allocated bytes and ps RSS are different measures.

Releasing off-screen SwiftUI roots recovered RSS, but its five-round cold-scroll
p95 was 38.693 [37.553–39.547] ms versus 28.019 [11.449–29.128] ms originally.
Instrumentation found zero height-cache misses during the scrolling sample:
the expensive first display was rebuilding selectable SwiftUI text layout,
with individual host layouts near 18 ms. A sizing-options pilot did not remove
that cost. Merely clearing roots also left empty hosting trees in AppKit's reuse
cache.

The final implementation uses selectable native NSTextFields directly for user
and assistant rows. Native immutable Markdown attributes and measured geometry
are shared from measurement to display; fragment runs, bold/code/italic/link
styles and Unicode remain supported. User bubbles draw in their cell, avoiding
an extra NSBox tree. Complex forms/tool items retain SwiftUI content only while
realized, and their complete hosts are removed on reuse. Measurements retain
models, attributes and scalar heights rather than full view/layout trees. The
native regression verifies selectable text and external-update anchor stability;
a separate native test checks Markdown attributes and fragment continuations.
The final workload ends with eight realized text cells for 50 table rows. A final live
heap capture found 9 CoreText lines/runs each, 109 attributed strings, 17
NSHostingViewBase objects and 34 NSImages; NSTextStorage remained one 640-byte
object. Allocated heap was 23.6 MiB, CoreAnimation resident dirty 976 KiB,
Malloc Small resident dirty 30.4 MiB, and post-heap vmmap physical footprint
46.0 MiB. Baseline allocated heap and footprint were 23.9 and 50.6 MiB.
These captures establish retained object
classes and regions, not allocator backtraces for every byte. The heap tool
itself changes residency, so the separately measured ps RSS remains authoritative
for the five-round memory comparison.

Earlier rejected approaches and all raw pilots remain in scratch: NSTextView
used 163 MiB RSS; an early SwiftUI-bridged NSTextField approach remained above
baseline. Direct native cells, releasing settled observers, avoiding duplicate
sidebar child rows and coalescing list snapshots are all included in the final
candidate. No model output is omitted to obtain the memory result.

## Final release measurements

Apple M3, 16 GiB, native arm64 macOS 27.2. Both arms use Release native apps,
release fixture helpers and identical monotonic measurement boundaries. The
baseline is `5610d9e021bbf83044a80974c7f6eac147fa2564` (PROTO 43). Five fresh
isolated homes per arm run AB, BA, AB, BA, AB. No build, test or profiler overlaps
these accepted rounds. Baseline executables were preserved; the final candidate
includes the switcher, family observers and all completed children.

Each real visible app spawns 32 children with 128 repeated output lines each,
then receives 16 external turns. Every run reaches parent seq 258, measures 48
store applications and 120 native scroll/layout/display blocks, and has a
scrollable document. Every candidate retains 33 family rows, all settled. The
same 900×532 window gives the original transcript a 410-point viewport and the
candidate about 270 points because its conversation list occupies space below
the composer. No child is hidden from that list to improve the result.

Values are median [min–max] across five per-run medians, or five per-run p95s
for the p95 rows; p95 uses nearest rank. RSS is the app's end-of-workload ps RSS,
excluding daemon/child processes.

| Metric | Baseline | Final candidate |
| --- | ---: | ---: |
| Main-thread store apply, µs | 25.292 [20.230–27.812] | 20.437 [16.875–21.062] |
| Native scroll/display work, ms | 3.373 [2.119–4.297] | 1.514 [1.418–1.567] |
| Native scroll/display work p95, ms | 27.127 [22.274–30.229] | 18.953 [18.628–19.177] |
| Display callback interval, ms | 16.675 [16.675–16.676] | 16.675 [16.673–16.677] |
| Display callback interval p95, ms | 43.432 [33.362–50.022] | 33.374 [33.360–33.712] |
| App RSS, MiB | 125.281 [123.969–125.656] | 125.172 [125.016–125.719] |
| Unwanted clip-origin change, points | 1371.500 [1335.000–1717.500] | 0.000 [0.000–0.000] |

Median RSS is 0.109 MiB below the contemporary baseline. The ranges overlap;
this is recovery to baseline, not evidence of a meaningful additional memory
saving. The earlier +30.906 MiB regression is recovered while including the
new navigation UI and shared family model. Native cold-scroll p95 is below
baseline in every candidate round (candidate maximum 19.177 ms versus baseline
minimum 22.274 ms), and median scroll work also improves. Exact anchor retention
holds in every candidate round. The first-layout tail does not regain all of
the eager full-page layout's 1.6 ms p95, which required retaining its costly text
and layer trees; it meets the original baseline with lower retained memory.

Raw accepted results and binary/helper/harness/product-diff hashes are in
`final-*/result.json`, `final-summary.json` and `final-provenance.json`.
`profile-final-after/ui.trace` was recorded successfully by command-line
Instruments against the real release app, separately from timed rounds, with
its table of contents exported. `heap-final-after` retains the live allocation
and vmmap captures. Earlier pilots, including the 7 ms drawn-cell pilot, are
not substituted for these five controlled final rounds.


Main-thread update timing is synchronous wall time around SessionStore.apply,
including identical signpost overhead; it is not exclusive CPU time or the
entire render pipeline. Native scroll work times scrolling, layout and
synchronous window display on the main actor, not GPU compositor completion.
CADisplayLink intervals separately measure callback delivery. Diagnostic heap
captures and command-line Instruments Time Profiler runs are outside timed rounds.
Production Session update, Transcript layout, row measurement/release/first-layout
signposts supply boundaries. No saving is a projection.

## White space: owner-pending attribution

The hierarchy has a 52-point toolbar region, a 25-point recap bar, and the
transcript at x=260, y=77, width=640, height=410 in a 900×532 content area.
The native clip is 623 points wide with a standard 17-point scroller. Source
inspection found no empty inspector, safeAreaInset or reserved overlay column.
The new transcript explicitly matches the native clip width.

System screen recording and accessibility permissions are absent. External
screencapture failed; in-process cacheDisplay captures blank/material surfaces
and even blank button labels, so its white pixels cannot establish the owner's
symptom. No system permission change was attempted. The owner's screenshot is
needed to identify the region before changing layout. Failure mode: an
unwanted reserved region may remain. Likelihood is owner-observed but not
locally quantified; repair cost is unknown until its producing view is known.

## Checks and evidence

All Cargo commands used `mise exec --`, jobs=2 and the one designated external
target directory. Build/test task equivalents redirected every native output
and generated package to scratch rather than creating worktree artifacts.

- Touched protocol/kernel/engine/client/ffi/tui/web nextest suites: **913 passed,
  20 skipped** (`final-nextest.log`).
- Required `cargo nextest run --workspace -E 'package(a)'`: **147 passed,
  10 skipped** (`final-package-a.log`), including real-process TUI/web cases.
- After the final web focus-hint adjustment, focused real web black-box/UI suite:
  **7 passed** (`final-web-focus.log`).
- Swift full suite: **40 tests completed**, with one external diagnostic test skipped
  (`final-swift.log`); final native current-marker regression passed separately
  (`final-marker-swift.log`).
- Final Clippy with warnings denied, rustfmt and `mise run check:windows` passed
  (`final-clippy.log`, `final-fmt-2.log`, `final-windows.log`).
- Release fixture helper, release BoltFFI package and native Release app built
  locally on macOS. Generated v45 wire corpus and view vectors passed their
  readers. Existing Xcode Rust archive alignment/deployment and CoreSimulator
  warnings remain in the logs; the macOS build succeeds.
- `git diff --check` passed. Full CI was **not dispatched**.

Earlier failing probes/test runs and corrections remain in raw logs: the first
full package(a) run exposed a navigation SSE patch arriving before the initial
transcript and an obsolete assertion that rejected separate navigation patches.
The page now queues family snapshots until its initial transcript, and the
long-transcript invariant still counts each transcript item exactly once. An
early child-stop UI probe allowed a fixture sleep to finish naturally; the final
real UI test instead cancels at the existing approval boundary.

SDK tests cover family ancestry, retained completed children, usage, elapsed
formatting, interrupted/failed states and activity refresh. Engine tests cover
execution spans across clock epochs. Shared vectors still verify one child
operation and old cuts. TUI tests cover keyboard selection, switching and stopping
in both terminal buffer modes. Swift live tests cover the head-only family even
when its child operation is outside demanded history, direct child attachment,
parent discovery, interruption and subsequent inbox messaging. A real NSWindow
checks arrow/Return/Escape navigation and current-marker updates without a new
head. There is no existing XCUITest suite. The real browser UI test launches two
background children, switches with keyboard, verifies child composition and
parent return, stops children and retains completed rows.

The stop tests use a real shell child waiting at the existing policy approval
boundary. An additional probe found that a currently executing non-cooperative
fixture sleep does not commit interruption until the tool returns. This is an
existing cancellation limitation; navigation uses the approved cancel path,
without changing engine execution/cancellation outside the allowed head scope.
Failure mode: stop can wait for such a tool to yield. Likelihood is confirmed
for the fixture sleep, not quantified for other tools. Fixing cooperative tool
cancellation would require separate execution-path scope and tests; cost has
not been measured. No speculative cancellation hardening was added.

Raw evidence lives under
`/private/tmp/claude-502/-Users-rajaniraiyn-work-a/57b9c5f0-cf3a-47ba-ba38-f417726d005b/scratchpad/r/app-ux`.
Baseline and initial fixed real-client evidence remains in `ffi-*.txt`,
`app-store.txt`, `native-view-tree.txt`, `scroll-positions.txt`, `web-events.txt`,
`tui-raw.txt` and `after-evidence/`. Initial timing/heap attribution remains in
`accepted-*`, `reuse-*`, `heap-before`, `heap-after`, `heap-memory-final-after`
and the earlier `profile-*/ui.trace` packages. Final evidence: `final-*/result.json`, `final-perf.log`, `final-summary.json`,
`final-provenance.json`, `heap-final-after/{heap,vmmap}.txt`,
`profile-final-after/ui.trace` and `final-*.log`.
All pilots, including failed approaches, are retained without discarding slow
rounds. Instrumentation/startup hooks and source mirrors remain external; they
are not shipped product UI.

All runtime environments were allowlisted, removing provider/proxy variables
before supplying dummy credentials, loopback endpoints and NO_PROXY. Only echo
fixtures ran. Neither `.env.local` nor the user's `~/.a` was read. Cargo ran
through mise with jobs=2 and the single external `tgt-app-ux`; native mirrors,
generated bindings and build artifacts stayed outside the worktree. Only owned
probe PIDs were stopped, and every isolated daemon uses its exact A_HOME with
`a daemon stop`. Evidence and the external target remain for review; phase-2
clean/deletion has not run. No history rewrite or child-storage redesign occurred.


## Phase 2: independent review fixes

Streaming reply deltas change `SessionStore.streaming` without changing committed
transcript entries. The native measurement cache therefore retained the reply's
initial height. Each entry now includes only its own active reply's streaming
text/attempt in measurement equality. Deltas invalidate that row's cached height
without changing its stable anchor ID or invalidating unrelated rows. A real
NSWindow regression feeds the shared recorded reducer stream, adds 30 lines with
no item change, and asserts the native row grows while following the bottom.
A second 30-line delta while scrolled up grows the row without moving the clip
origin. This regression timed out on the previous implementation and passes
with the fix (`p2-stream-red.log`, `p2-swift.log`).

The TUI retained a selected array index across family snapshots. A grandchild
inserted before child B moved keyboard actions to that new row. Snapshot delivery
now resolves the previously selected SessionId in the new list before replacing
its rows. The invariant test checks both alternate-screen and inline modes and
asserts x and Enter still target B after insertion (`p2-selection-red.log`,
`p2-nextest.log`). It fails against the preceding implementation.

The macOS switcher already saves the selected SessionId before reload and finds
its new row afterward. Its native keyboard test now spawns two children, selects
B, creates A's grandchild through A's actual inbox, and checks selection moves
to B's new row before Return activates B. Web rows likewise use stable
session-derived DOM IDs and actions read the focused row's session ID. The
browser regression opens A in a second authenticated tab, inserts its grandchild
before focused B, and checks focus, roving tabindex and Enter still target B.
No production selection change was necessary in these two clients.

Local verification: 134 TUI/web nextest tests passed (3 skipped); the complete
Swift suite completed 41 tests (one external diagnostic skipped), including the
new streaming and native insertion checks. Clippy, formatting, the native Release
build and the Windows lint passed. Initial extended browser runs failed test setup while establishing a shared
authenticated context; the corrected test creates an explicit browser context
and both tabs share it. The focused browser journey passes (`p2-web-3.log`). The complete package(a) result and hosted CI
result are recorded below when complete. Phase-1 performance numbers above refer
to its measured candidate; they were not rerun for these two correctness fixes.
